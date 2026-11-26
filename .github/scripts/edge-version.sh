#!/usr/bin/env bash
# Prints the edge version for the checked-out commit: the patch after the
# last release in .release-please-manifest.json, with the number of commits
# reachable from HEAD as the prerelease number (e.g. 1.7.1-edge.1234).
#
# The result sorts above the last release and below any release that follows
# it, and grows with every commit on main. Needs full history, so check out
# with `fetch-depth: 0`.
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "$(git rev-parse --is-shallow-repository)" == "true" ]]; then
  echo "edge-version: shallow clone; check out with fetch-depth: 0" >&2
  exit 1
fi

base="$(jq -r '."."' .release-please-manifest.json)"
IFS=. read -r major minor patch <<< "$base"
echo "${major}.${minor}.$((patch + 1))-edge.$(git rev-list --count HEAD)"
