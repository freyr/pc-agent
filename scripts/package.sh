#!/usr/bin/env bash
# Package the release binary for the x86_64 Linux release.
#
# Usage: bash scripts/package.sh <output-dir>
# Run `cargo build --release` first. Writes two files to <output-dir>:
#   pc-agent-x86_64-linux.tar.gz         (pc-agent and README.md at the top level)
#   pc-agent-x86_64-linux.tar.gz.sha256  (output of sha256sum)

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: bash $0 <output-dir>" >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$1"
out="$(cd "$1" && pwd)"
name="pc-agent-x86_64-linux.tar.gz"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

install -m 0755 "$root/target/release/pc-agent" "$stage/pc-agent"
install -m 0644 "$root/README.md" "$stage/README.md"

tar -C "$stage" -czf "$out/$name" pc-agent README.md
(cd "$out" && sha256sum "$name" > "$name.sha256")
