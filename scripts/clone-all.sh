#!/usr/bin/env bash
# Clone (or fast-forward) every sibling repo next to this one, which is where
# the workspace path dependencies in ../Cargo.toml expect to find them.
#
# Usage: scripts/clone-all.sh [git-remote-prefix]
#   e.g. scripts/clone-all.sh git@github.com:YOURORG
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
prefix="${1:-${LUMEN_REMOTE:-}}"

if [ -z "$prefix" ]; then
  echo "usage: $0 <git-remote-prefix>   (or set LUMEN_REMOTE)" >&2
  exit 2
fi

while read -r repo; do
  [ -n "$repo" ] || continue
  if [ -d "$root/$repo/.git" ]; then
    echo "== $repo (pull)"
    git -C "$root/$repo" pull --ff-only
  else
    echo "== $repo (clone)"
    git clone "$prefix/$repo.git" "$root/$repo"
  fi
done < "$here/repos.txt"
