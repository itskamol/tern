#!/usr/bin/env bash
# The release-time memory check: measures tern's scenes in a copied bundle and fails when a median
# is over its ceiling (#22; targets in #12, method in #7). Replaces scripts/measure-idle.sh.
#
#   scripts/measure-memory.sh [path/to/tern.app] [runs]   (default target/release/bundle/tern.app, 3)
#
# Exit status: 0 every median is within its ceiling, 1 one is over, 2 a reading was refused or the
# setup failed (the reason is printed). Targets are printed too; a median over its target but
# within its ceiling is marked "over target" and does not fail.
#
# Scenes, each the median of `runs` runs of a fresh copy of the bundle, window 1320x880:
#   no tab      nothing restored, no wallpaper (there is none)
#   one tab     `tern demo-cmd`: a tab to the local demo server, logged in by an approved password
#               command, with a pre-seeded known-hosts file
#   peak        phys_footprint_peak of the runs of the two scenes above, pooled
#   vault peak  phys_footprint_peak with a throwaway vault opened at start-up (see below)
# Each run waits $TERN_MEASURE_SETTLE seconds (30) after launch, then reads `footprint -p`.
#
# Refused, not measured: nothing is drawn while the screen is locked or when tern's window is
# covered or off screen, so every 2 s of the wait (and at the reading) the screen must be unlocked
# and at least half of the copy's window must be uncovered by other apps' windows
# (scripts/measure-window.swift --visible). The copy need not be the front app: a script cannot
# make it so without an Accessibility grant, and a window that shows is drawn either way. Run it
# with the Mac awake and nothing over the middle of the screen; it holds the display awake itself.
#
# Safe beside the person's tern: each copy has its own TERN_CONFIG_DIR (so its own single-instance
# lock), its own TERN_KNOWN_HOSTS and no Keychain use (the opt-in is off in the seeded settings),
# and only processes this script started are ever stopped, by pid. The demo server listens on
# 127.0.0.1:$TERN_MEASURE_PORT (2417), not the port scripts/dev-check.sh uses.
#
# The vault scene needs tern to unlock a vault without anyone typing, and nothing in a shipped
# build can: a typed passphrase is the only way in that is not the person's Keychain. So that one
# scene runs a copy whose binary was built from the same tree with `--features dev-keys`, which
# compiles in the TERN_DEV_KEYS replay (crates/tern/src/devkeys.rs) that types the passphrase of a
# throwaway vault (crates/tern/examples/throwaway_vault.rs) into the prompt of a host with a
# vault key. The other scenes run the bundle as shipped, and the script refuses a bundle whose
# binary carries the replay. The variant build overwrites target/release/tern, which the next
# scripts/bundle-app.sh rebuilds.
set -euo pipefail
cd "$(dirname "$0")/.."

bundle=${1:-target/release/bundle/tern.app}
runs=${2:-3}
port=${TERN_MEASURE_PORT:-2417}
settle=${TERN_MEASURE_SETTLE:-30}
window=1320x880
passphrase=measurepass1
# Opening a vault allocates the scrypt block (2^18 x 8 x 128 B = 256 MiB): a peak under it means
# the vault never opened and the scene would pass for the wrong reason.
scrypt_floor=256

[ -d "$bundle" ] || { echo "no app at $bundle (scripts/bundle-app.sh builds one)" >&2; exit 2; }
bundle=$(cd "$bundle" && pwd)
[ "$runs" -ge 1 ] 2>/dev/null || { echo "runs must be a positive number" >&2; exit 2; }
if grep -aq TERN_DEV_KEYS "$bundle/Contents/MacOS/tern"; then
  echo "refused: $bundle carries the dev-keys replay; measure the bundle as shipped" >&2
  exit 2
fi

work=$(mktemp -d)
tern_pid=""
server_pid=""
stop_all() {
  [ -z "$tern_pid" ] || kill "$tern_pid" 2>/dev/null || true
  [ -z "$server_pid" ] || kill "$server_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap stop_all EXIT
refuse() { echo "refused: $*" >&2; exit 2; }

caffeinate -di -w $$ &

echo "building the helpers" >&2
cargo build -q -p tern-ssh --example password_server
cargo build -q -p tern --example throwaway_vault
cargo build -q --release -p tern --features dev-keys
cp target/release/tern "$work/tern-dev-keys"
grep -aq TERN_DEV_KEYS "$work/tern-dev-keys" || refuse "the dev-keys build lacks the replay"
swiftc -O -o "$work/window" scripts/measure-window.swift

target/debug/examples/password_server --demo "$port" hunter2 >"$work/server.log" 2>&1 &
server_pid=$!
known_hosts="$work/known_hosts"
for _ in $(seq 1 30); do
  ssh-keyscan -p "$port" 127.0.0.1 >"$known_hosts" 2>/dev/null || true
  [ -s "$known_hosts" ] && break
  sleep 0.5
done
[ -s "$known_hosts" ] || refuse "the demo server did not answer on port $port ($work/server.log)"

# --- readings ----------------------------------------------------------------------------------

screen_locked() { [ "$( (ioreg -n Root -d1 | grep -c CGSSessionScreenIsLocked) || true)" != 0 ]; }
# Refuses the sample unless the screen is unlocked and at least half of tern's window shows.
ready() {
  screen_locked && refuse "the screen is locked ($1)"
  kill -0 "$tern_pid" 2>/dev/null || refuse "tern exited ($1)"
  local shown
  shown=$("$work/window" --visible "$tern_pid" 2>/dev/null || echo 0)
  [ "${shown:-0}" -ge 50 ] || refuse "tern's window is covered or off screen, ${shown:-0}% shows ($1)"
}

# One `phys_footprint` line of `footprint -p`, in MB.
footprint_mb() {
  footprint -p "$tern_pid" 2>/dev/null | awk -v k="$1:" '
    $1 == k { v = $2; u = $3
      if (u ~ /^KB/) v /= 1024; else if (u ~ /^GB/) v *= 1024
      printf "%.1f\n", v; exit }'
}

established() { lsof -a -p "$tern_pid" -iTCP:"$port" -sTCP:ESTABLISHED 2>/dev/null | tail -n +2 | wc -l | tr -d ' '; }

# --- one run -----------------------------------------------------------------------------------

# run SCENE N: launches a copy, waits, reads. Appends "foot peak" to $work/SCENE.
run() {
  local scene=$1 n=$2 dir="$work/$1-$2" app config
  app="$dir/tern.app"
  config="$dir/config"
  mkdir -p "$config"
  ditto "$bundle" "$app"
  local envs=(--env TERN_CONFIG_DIR="$config" --env TERN_KNOWN_HOSTS="$known_hosts") args=()
  case $scene in
    bare) ;;
    tab | vault)
      printf '{"vaultKeychain":false}\n' >"$config/settings.json"
      printf '["printf hunter2"]\n' >"$config/approved-commands.json"
      local name=demo-cmd extra=""
      if [ "$scene" = vault ]; then
        name=demo-vault extra=',"vaultKey":"bench"'
        cp "$work/tern-dev-keys" "$app/Contents/MacOS/tern"
        codesign -s - --force "$app" 2>/dev/null
        target/debug/examples/throwaway_vault "$config/vault.age" "$passphrase"
        envs+=(--env TERN_DEV_KEYS="sleep-2000 type:$passphrase enter")
      fi
      printf '{"version":1,"connections":[{"name":"%s","host":"127.0.0.1","port":%s,"user":"test","passwordCommand":"printf hunter2"%s}]}\n' \
        "$name" "$port" "$extra" >"$config/hosts.json"
      args=(--args "$name") ;;
  esac
  open -n "${envs[@]}" "$app" ${args[@]+"${args[@]}"}
  tern_pid=""
  for _ in $(seq 1 40); do
    tern_pid=$(pgrep -f "$app/Contents/MacOS/tern" | head -n 1 || true)
    [ -n "$tern_pid" ] && break
    sleep 0.25
  done
  [ -n "$tern_pid" ] || refuse "tern did not start ($scene run $n)"

  local at="$scene run $n" waited=0
  while [ "$waited" -lt "$settle" ]; do
    sleep 2
    waited=$((waited + 2))
    # The window needs a moment to appear; from 6 s on it must show.
    [ "$waited" -lt 6 ] || ready "$at, ${waited}s in"
  done
  ready "$at, at the reading"
  local foot peak size
  foot=$(footprint_mb phys_footprint)
  peak=$(footprint_mb phys_footprint_peak)
  size=$("$work/window" "$tern_pid" | awk -Fx '{ print $1 * $2, $0 }' | sort -n | tail -n 1 | cut -d' ' -f2)
  ready "$at, after the reading"
  [ -n "$foot" ] && [ -n "$peak" ] || refuse "footprint gave no reading ($at)"
  [ "$size" = "$window" ] || refuse "the window is ${size:-missing}, not $window ($at)"
  if [ "$scene" != bare ] && [ "$(established)" -lt 1 ]; then
    refuse "the tab did not connect to the demo server ($at)"
  fi
  if [ "$scene" = vault ] && ! awk -v p="$peak" -v f="$scrypt_floor" 'BEGIN { exit !(p >= f) }'; then
    refuse "peak ${peak} MB is under the scrypt block, so the vault did not open ($at)"
  fi
  echo "$scene run $n: ${foot} MB footprint, ${peak} MB peak" >&2
  echo "$foot $peak" >>"$work/$scene"
  kill "$tern_pid" 2>/dev/null || true
  for _ in $(seq 1 20); do kill -0 "$tern_pid" 2>/dev/null || break; sleep 0.25; done
  tern_pid=""
  rm -rf "$dir"
}

for scene in bare tab vault; do
  for ((n = 1; n <= runs; n++)); do run "$scene" "$n"; done
done

# --- the table ---------------------------------------------------------------------------------

median() { sort -n | awk '{ a[NR] = $1 } END { print a[int((NR + 1) / 2)] }'; }
range() { sort -n | awk 'NR == 1 { lo = $1 } { hi = $1 } END { print lo "-" hi }'; }
column_of() { awk -v c="$2" '{ print $c }' "$work/$1"; }

rows=""
status=0
add() { # label target ceiling values-on-stdin
  local label=$1 target=$2 ceiling=$3 vals med rng verdict
  vals=$(cat)
  med=$(printf '%s\n' "$vals" | median)
  rng=$(printf '%s\n' "$vals" | range)
  verdict=ok
  if awk -v m="$med" -v c="$ceiling" 'BEGIN { exit !(m > c) }'; then
    verdict="OVER CEILING"
    status=1
  elif awk -v m="$med" -v t="$target" 'BEGIN { exit !(m > t) }'; then
    verdict="over target"
  fi
  rows="$rows$(printf '%-24s %7s %8s %8s %-11s %s' "$label" "$target" "$ceiling" "$med" "$rng" "$verdict")"$'\n'
  summary="$summary | $med ($rng)"
}
summary=""
add "no tab" 70 80 < <(column_of bare 1)
add "one connected tab" 75 85 < <(column_of tab 1)
add "start-up peak, no vault" 90 100 < <(column_of bare 2; column_of tab 2)
add "start-up peak, vault" 380 400 < <(column_of vault 2)

echo
printf '%-24s %7s %8s %8s %-11s %s\n' scene target ceiling median range verdict
printf '%s' "$rows"
echo "(MB, phys_footprint; ${runs} runs each, window $window, vault scene on the dev-keys variant)"
echo
echo "docs/perf.md row:"
echo "| date | commit | no tab | one tab | peak | vault peak |"
echo "| $(date +%Y-%m-%d) | $(git rev-parse --short HEAD)$summary |"
exit "$status"
