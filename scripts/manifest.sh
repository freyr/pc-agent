#!/usr/bin/env bash
# Write the release manifest of the bare binaries in a folder to stdout.
#
# Usage: bash scripts/manifest.sh <dir> > manifest.json
# <dir> must hold the three bare binaries:
#   pc-agent-x86_64-linux, pc-agent-aarch64-macos, pc-agent-x86_64-windows.exe
# Run it in a git checkout of the release commit:
#   version   the version in Cargo.toml
#   commit    the full commit of HEAD
#   built_at  the commit time of HEAD in UTC (RFC 3339), the same rule as
#             build.rs, so the manifest and the binaries have the same value.
# Sign the output with minisign; the agent reads it only after the signature
# check (src/update).

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: bash $0 <dir> > manifest.json" >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dir="$1"

version="$(awk '/^\[/ { in_pkg = ($0 == "[package]") } in_pkg && /^version *=/ { gsub(/.*= *"|".*/, ""); print; exit }' "$root/Cargo.toml")"
commit="$(git -C "$root" rev-parse HEAD)"
seconds="$(git -C "$root" log -1 --format=%ct HEAD)"
if date -u -d "@0" >/dev/null 2>&1; then
  built_at="$(date -u -d "@$seconds" +%Y-%m-%dT%H:%M:%SZ)"
else
  built_at="$(date -u -r "$seconds" +%Y-%m-%dT%H:%M:%SZ)"
fi

sha256() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

entries=""
for pair in x86_64-linux:pc-agent-x86_64-linux \
  aarch64-macos:pc-agent-aarch64-macos \
  x86_64-windows:pc-agent-x86_64-windows.exe; do
  platform="${pair%%:*}"
  asset="${pair#*:}"
  if [[ ! -f "$dir/$asset" ]]; then
    echo "Missing asset: $dir/$asset" >&2
    exit 1
  fi
  entries+="${entries:+,
}    \"$platform\": { \"asset\": \"$asset\", \"sha256\": \"$(sha256 "$dir/$asset")\" }"
done

cat <<EOF
{
  "version": "$version",
  "commit": "$commit",
  "built_at": "$built_at",
  "platforms": {
$entries
  }
}
EOF
