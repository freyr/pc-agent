#!/usr/bin/env bash
# CI check of the signed release manifest on Linux and macOS.
#
# Usage: bash scripts/check-manifest.sh <dist-dir>
# Run `cargo build --release` and scripts/package.sh <dist-dir> first; needs
# minisign. Makes a test key, writes the manifest with scripts/manifest.sh
# (the bare binary of this OS and placeholder files for the other two), signs
# it, and checks with `pc-agent update --verify` that:
#   - the signed manifest verifies,
#   - its commit and built_at are the ones that build.rs embedded,
#   - a changed manifest and a different key fail.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: bash $0 <dist-dir>" >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist="$(cd "$1" && pwd)"
agent="$root/target/release/pc-agent"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mkdir "$work/assets"
for asset in pc-agent-x86_64-linux pc-agent-aarch64-macos pc-agent-x86_64-windows.exe; do
  if [[ -f "$dist/$asset" ]]; then
    cp "$dist/$asset" "$work/assets/$asset"
  else
    echo "placeholder for $asset" > "$work/assets/$asset"
  fi
done

bash "$root/scripts/manifest.sh" "$work/assets" > "$work/manifest.json"
cat "$work/manifest.json"
minisign -G -W -p "$work/test.pub" -s "$work/test.key" > /dev/null
minisign -S -s "$work/test.key" -m "$work/manifest.json" -x "$work/manifest.json.minisig"

out="$("$agent" update --verify "$work/manifest.json" "$work/manifest.json.minisig" "$work/test.pub")"
echo "$out"
field() { sed -n "s/^$1: //p" <<< "$out"; }
if [[ "$(field 'manifest built_at')" != "$(field 'running built_at')" ]]; then
  echo "::error::built_at of the manifest and of the binary differ" >&2
  exit 1
fi
if [[ "$(field 'manifest commit')" != "$(field 'running commit')" ]]; then
  echo "::error::commit of the manifest and of the binary differ" >&2
  exit 1
fi

sed 's/"version": "/"version": "9/' "$work/manifest.json" > "$work/changed.json"
if cmp -s "$work/manifest.json" "$work/changed.json"; then
  echo "::error::the change of the manifest did not apply" >&2
  exit 1
fi
if "$agent" update --verify "$work/changed.json" "$work/manifest.json.minisig" "$work/test.pub"; then
  echo "::error::a changed manifest verified" >&2
  exit 1
fi

minisign -G -W -p "$work/other.pub" -s "$work/other.key" > /dev/null
if "$agent" update --verify "$work/manifest.json" "$work/manifest.json.minisig" "$work/other.pub"; then
  echo "::error::the manifest verified with a different key" >&2
  exit 1
fi
echo "The manifest check passed."
