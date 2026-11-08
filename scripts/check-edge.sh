#!/usr/bin/env bash
# CI check of `pc-agent update --check --channel edge` against the real edge
# release on Linux and macOS.
#
# Usage: bash scripts/check-edge.sh <pc-agent binary>
# The check must exit 0 (up to date or an update available) and print the
# commit of the live edge manifest. The release workflow of the same push can
# replace the edge release during this run (the manifest is missing for a
# short time), so the check tries again a few times.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: bash $0 <pc-agent binary>" >&2
  exit 1
fi

exe="$1"
url="https://github.com/freyr/pc-agent/releases/download/edge/manifest.json"

for attempt in 1 2 3 4 5; do
  echo "== attempt $attempt"
  if out="$("$exe" update --check --channel edge 2>&1)"; then
    echo "$out"
    expected="$(curl -fsSL "$url" | sed -n 's/.*"commit": *"\([0-9a-f]\{40\}\)".*/\1/p')" || true
    if [[ -n "$expected" ]] && grep -q "^manifest: .* commit $expected " <<<"$out"; then
      echo "OK: the check prints the edge commit $expected"
      exit 0
    fi
    echo "the output does not have the edge commit '$expected'"
  else
    echo "$out"
  fi
  sleep 30
done
echo "update --check --channel edge failed" >&2
exit 1
