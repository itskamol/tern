#!/usr/bin/env bash
# Frames and window memory while you (or a script) drive tern: runs a tern binary as the dev-check
# app with frame stats on, samples its IOSurface once a second, and prints the busy seconds.
# For checks like #20's (docs/perf.md): run it once per build being compared, doing the same
# scenes in each (the demo shell's `flood`, a split divider drag, a window resize).
#
#   scripts/measure-frames.sh <tern binary> <seconds> [label]
#
# - Run scripts/dev-check.sh once first: it makes TernDev.app, the config and the demo server.
# - The window must be in view, or nothing is drawn and the stats read about 1 frame/s
#   (swift scripts/onscreen-windows.swift tells). A release build measures what ships; build it
#   with `--features dev-keys` to drive the scenes with TERN_DEV_KEYS (see devkeys.rs).
# - Extra environment for the app goes in MEASURE_ENV, e.g. MEASURE_ENV="TERN_DEV_KEYS=enter".
# - Output: target/dev-check/frames-<label>.log (the app's log) and .foot (the samples).
set -euo pipefail
cd "$(dirname "$0")/.."

bin=${1:?usage: measure-frames.sh <tern binary> <seconds> [label]}
secs=${2:?usage: measure-frames.sh <tern binary> <seconds> [label]}
label=${3:-run}
root=target/dev-check
app="$root/TernDev.app"
config="$PWD/$root/config"
out="$root/frames-$label"
[ -d "$app" ] && [ -d "$config" ] || { echo "run scripts/dev-check.sh first" >&2; exit 1; }

pkill -f "$app/Contents/MacOS/tern" || true
sleep 1
cp "$bin" "$app/Contents/MacOS/tern"
codesign -s - --force "$app" 2>/dev/null

envs=(--env TERN_FRAME_STATS=1 --env TERN_CONFIG_DIR="$config"
  --env TERN_KNOWN_HOSTS="$PWD/$root/known_hosts")
for kv in ${MEASURE_ENV:-}; do envs+=(--env "$kv"); done
open -n --stderr "$PWD/$out.log" --stdout /dev/null "${envs[@]}" "$app"
sleep 2
pid=$(pgrep -f "$app/Contents/MacOS/tern" | head -1)
echo "tern pid $pid: sampling for ${secs}s; do the scenes now"

: > "$out.foot"
for i in $(seq 1 "$secs"); do
  sleep 1
  footprint -p "$pid" 2>/dev/null | awk -v t="$i" '/IOSurface/ {print t, $1; exit}' >> "$out.foot"
done
footprint -p "$pid" 2>/dev/null | awk '/phys_footprint:/ {print "footprint", $2, $3; exit}' >> "$out.foot"

echo "Seconds with more than 2 frames (render and terminal prepaint, microseconds):"
jq -r 'select(.fields.message == "frame_stats" and .fields.frames > 2)
  | .fields as $f
  | "\(.timestamp[11:19])  frames \($f.frames)  render mean/max \($f.render_mean_us)/\($f.render_max_us)  prepaint mean/max \($f.prepaint_mean_us)/\($f.prepaint_max_us)"' \
  "$out.log" 2>/dev/null || true
awk '$1 != "footprint" {n++; v=$2+0; if (min=="" || v<min) min=v; if (v>max) max=v}
  $1 == "footprint" {fp=$2" "$3}
  END {printf "IOSurface over %d samples: %s-%s MB; footprint at the end %s\n", n, min, max, fp}' "$out.foot"
