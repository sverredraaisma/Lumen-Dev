#!/usr/bin/env bash
# Run a command in every sibling repo, including this one.
#
#   scripts/foreach.sh git status --short
#   scripts/foreach.sh cargo test --workspace
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
status=0

for repo in $(cat "$here/repos.txt") lumen-dev; do
  [ -d "$root/$repo" ] || continue
  echo "== $repo"
  ( cd "$root/$repo" && "$@" ) || status=1
done
exit "$status"
