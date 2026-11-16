#!/usr/bin/env bash
# Says whether a pull request on the fork is safe to merge: merges it into the fork's main in a
# throwaway worktree and runs the push gates there. GitHub runs no local hook on a merge, and
# the fork's CI does not run, so this is the only gate a merge has.
#
#   scripts/check-merge.sh 56
#
# Nothing in this checkout changes: the worktree lives in target/check-merge and is removed at
# the end. It builds into this checkout's target/, so dependencies are not built twice.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pr=${1:?usage: scripts/check-merge.sh <pull request number>}
root=$PWD
tree=$root/target/check-merge

git worktree remove --force "$tree" 2>/dev/null || true
trap 'git -C "$root" worktree remove --force "$tree" 2>/dev/null || true' EXIT

git fetch -q fork main
base=$(git rev-parse FETCH_HEAD)
git fetch -q fork "pull/$pr/head"
head=$(git rev-parse FETCH_HEAD)

git worktree add -q --detach "$tree" "$base"
cd "$tree"
if ! git merge -q --no-ff --no-edit "$head"; then
  echo "check-merge: #$pr does not merge cleanly into main:"
  git diff --name-only --diff-filter=U | sed 's/^/  /'
  exit 1
fi
merged=$(git rev-parse HEAD)

# pre-push reads the pushed refs on stdin; this line makes it see the merge as a push to main,
# so its changed-line mutation check covers what the pull request changes.
mkdir -p target
printf 'refs/heads/check-merge %s refs/heads/main %s\n' "$merged" "$base" \
  | CARGO_TARGET_DIR="$root/target" scripts/hooks/pre-push

repo=$(git remote get-url fork | sed -E 's#^.*github\.com[:/]##; s#\.git$##')
echo "check-merge: #$pr passes on top of main ($(git rev-parse --short "$base")). To merge:"
echo "  gh pr merge $pr --repo $repo --merge --delete-branch"
