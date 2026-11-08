#!/usr/bin/env bash
# Package the release binary for the release of this OS and CPU.
#
# Usage: bash scripts/package.sh <output-dir>
# Run `cargo build --release` first. The target name comes from the OS:
#   Linux x86_64  -> x86_64-linux
#   macOS arm64   -> aarch64-macos
# Writes three files to <output-dir>:
#   pc-agent-<target>                (the bare binary, for the self-update; see
#                                     scripts/manifest.sh)
#   pc-agent-<target>.tar.gz         (pc-agent, README.md and packaging/config.example.toml;
#                                     on macOS also packaging/pc-agent.plist and
#                                     scripts/install-macos.sh; on Linux also
#                                     packaging/pc-agent.service and scripts/install.sh)
#   pc-agent-<target>.tar.gz.sha256  (checkable with sha256sum -c or shasum -a 256 -c)

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: bash $0 <output-dir>" >&2
  exit 1
fi

os="$(uname -s)"
cpu="$(uname -m)"
case "$os/$cpu" in
  Linux/x86_64) target="x86_64-linux" ;;
  Darwin/arm64) target="aarch64-macos" ;;
  *)
    echo "Unsupported platform: $os $cpu" >&2
    exit 1
    ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$1"
out="$(cd "$1" && pwd)"
name="pc-agent-$target.tar.gz"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

mkdir "$stage/packaging"
install -m 0755 "$root/target/release/pc-agent" "$stage/pc-agent"
install -m 0644 "$root/README.md" "$stage/README.md"
install -m 0644 "$root/packaging/config.example.toml" "$stage/packaging/config.example.toml"
files=(pc-agent README.md packaging/config.example.toml)
if [[ "$os" == "Darwin" ]]; then
  install -m 0644 "$root/packaging/pc-agent.plist" "$stage/packaging/pc-agent.plist"
  files+=(packaging/pc-agent.plist)
  mkdir "$stage/scripts"
  install -m 0755 "$root/scripts/install-macos.sh" "$stage/scripts/install-macos.sh"
  files+=(scripts/install-macos.sh)
else
  install -m 0644 "$root/packaging/pc-agent.service" "$stage/packaging/pc-agent.service"
  files+=(packaging/pc-agent.service)
  mkdir "$stage/scripts"
  install -m 0755 "$root/scripts/install.sh" "$stage/scripts/install.sh"
  files+=(scripts/install.sh)
fi

tar -C "$stage" -czf "$out/$name" "${files[@]}"
install -m 0755 "$root/target/release/pc-agent" "$out/pc-agent-$target"
if [[ "$os" == "Darwin" ]]; then
  (cd "$out" && shasum -a 256 "$name" > "$name.sha256")
else
  (cd "$out" && sha256sum "$name" > "$name.sha256")
fi
