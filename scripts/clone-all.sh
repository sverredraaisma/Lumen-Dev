#!/usr/bin/env bash
# Clone (or fast-forward) every sibling repo next to this one, which is where
# the workspace path dependencies in ../Cargo.toml expect to find them.
#
# Usage: scripts/clone-all.sh [git-remote-prefix]
#   e.g. scripts/clone-all.sh https://github.com/sverredraaisma
#        scripts/clone-all.sh git@github.com:sverredraaisma
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
prefix="${1:-${LUMEN_REMOTE:-https://github.com/sverredraaisma}}"

prefix="${prefix%/}"

# The clone target directory is the LOCAL name, never the remote one. Sibling
# manifests resolve `../lumen-core` literally, and on a case-sensitive
# filesystem a checkout named `Lumen-Core` fails at manifest parse rather than
# at anything that would tell you why.
while read -r local remote; do
  case "$local" in ''|\#*) continue ;; esac
  [ -n "$remote" ] || { echo "repos.txt: no remote name for '$local'" >&2; exit 2; }

  if [ -d "$root/$local/.git" ]; then
    echo "== $local (pull)"
    git -C "$root/$local" pull --ff-only
  else
    echo "== $local (clone $remote)"
    git clone "$prefix/$remote.git" "$root/$local"
  fi
done < "$here/repos.txt"
