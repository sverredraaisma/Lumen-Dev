#!/usr/bin/env bash
# Run a command in every sibling repo, including this one.
#
#   scripts/foreach.sh git status --short
#   scripts/foreach.sh cargo test --workspace
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
status=0

# First column only: repos.txt maps a local directory name to a remote
# repository name, and it is the local one that exists on disk.
repos=()
while read -r local _remote; do
  case "$local" in ''|\#*) continue ;; esac
  repos+=("$local")
done < "$here/repos.txt"
repos+=(lumen-dev)

for repo in "${repos[@]}"; do
  [ -d "$root/$repo" ] || continue
  echo "== $repo"
  ( cd "$root/$repo" && "$@" ) || status=1
done
exit "$status"
