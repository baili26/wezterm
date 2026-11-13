#!/bin/sh
# Sync the official upstream wezterm into this personal fork.
#
#   main   - pristine mirror of upstream/main (fast-forward only)
#   local  - personal feature branch, rebased onto main
#
# Usage: scripts/sync-upstream.sh

set -eu

cd "$(git rev-parse --show-toplevel)"

if ! git remote get-url upstream >/dev/null 2>&1; then
    echo "adding upstream remote"
    git remote add upstream https://github.com/wezterm/wezterm.git
fi

echo "fetching upstream..."
git fetch --tags upstream

echo "updating main (fast-forward only)..."
git switch main
if ! git merge --ff-only upstream/main; then
    echo
    echo "ERROR: main is not a fast-forward of upstream/main." >&2
    echo "main must stay a pristine mirror. Fix it manually, then re-run." >&2
    exit 1
fi

echo "rebasing local onto main..."
git switch local
git rebase main

echo
echo "synced. main is now:"
git --no-pager log -1 --format='  %h %s' main
echo "local is now:"
git --no-pager log --oneline "main..local"
