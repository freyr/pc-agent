#!/usr/bin/env bash
# Build the macOS installer package.
#
# Usage: bash scripts/build-pkg.sh <output-dir>
# Run `cargo build --release` first. Needs macOS (pkgbuild, productbuild).
# Writes two files to <output-dir>:
#   pc-agent-<version>-macos.pkg
#   pc-agent-<version>-macos.pkg.sha256  (checkable with shasum -a 256 -c)
# The package is not signed.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: bash $0 <output-dir>" >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script is for macOS only." >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ! -x "$root/target/release/pc-agent" ]]; then
  echo "Run cargo build --release first." >&2
  exit 1
fi
mkdir -p "$1"
out="$(cd "$1" && pwd)"

# The version is the first version line of the [package] section.
version="$(awk '/^\[/ { in_pkg = ($0 == "[package]") } in_pkg && /^version *=/ { gsub(/.*= *"|".*/, ""); print; exit }' "$root/Cargo.toml")"
if [[ -z "$version" ]]; then
  echo "Found no version in Cargo.toml." >&2
  exit 1
fi
name="pc-agent-$version-macos.pkg"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
stage="$tmp/root"

mkdir -p "$stage/usr/local/bin" "$stage/Library/LaunchDaemons" "$stage/usr/local/share/pc-agent"
install -m 0755 "$root/target/release/pc-agent" "$stage/usr/local/bin/pc-agent"
install -m 0644 "$root/packaging/pc-agent.plist" "$stage/Library/LaunchDaemons/pc-agent.plist"
install -m 0644 "$root/packaging/config.example.toml" "$stage/usr/local/share/pc-agent/config.example.toml"
install -m 0755 "$root/packaging/macos/uninstall.sh" "$stage/usr/local/share/pc-agent/uninstall.sh"

# Copy the scripts so the modes are right whatever the checkout did.
mkdir "$tmp/scripts"
install -m 0755 "$root/packaging/macos/scripts/preinstall" "$tmp/scripts/preinstall"
install -m 0755 "$root/packaging/macos/scripts/postinstall" "$tmp/scripts/postinstall"

# Extended attributes would become ._* files in the payload.
xattr -cr "$stage" "$tmp/scripts"
export COPYFILE_DISABLE=1

pkgbuild \
  --root "$stage" \
  --scripts "$tmp/scripts" \
  --identifier com.github.freyr.pc-agent \
  --version "$version" \
  --install-location / \
  "$tmp/pc-agent-component.pkg"

# The binary is arm64 only: restrict the product to Apple silicon.
productbuild --synthesize --package "$tmp/pc-agent-component.pkg" "$tmp/distribution.xml"
sed -i '' 's/hostArchitectures="[^"]*"/hostArchitectures="arm64"/' "$tmp/distribution.xml"
grep -q 'hostArchitectures="arm64"' "$tmp/distribution.xml"
productbuild --distribution "$tmp/distribution.xml" --package-path "$tmp" "$out/$name"

# AppleDouble entries come from extended attributes that this Mac cannot
# remove (com.apple.provenance). Fail in CI, warn on a local build.
if grep -q '/\._' < <(pkgutil --payload-files "$out/$name"); then
  if [[ -n "${CI:-}" ]]; then
    echo "The payload has ._* entries." >&2
    exit 1
  fi
  echo "Warning: the payload has ._* entries (local build). Use the CI package." >&2
fi
(cd "$out" && shasum -a 256 "$name" > "$name.sha256")
echo "Built $out/$name"
