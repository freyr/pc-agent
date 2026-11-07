#!/usr/bin/env bash
# Build pc-agent and install it as a LaunchDaemon on macOS.
#
# Usage:
#   sudo scripts/install-macos.sh              build and install
#   sudo scripts/install-macos.sh --uninstall  stop the service, remove the plist and the binary
#
# The build runs as the calling user (SUDO_USER). The configuration file
# /etc/pc-agent/config.toml is never overwritten and never removed.

set -euo pipefail

label="pc-agent"
bin="/usr/local/bin/pc-agent"
plist="/Library/LaunchDaemons/pc-agent.plist"
conf_dir="/etc/pc-agent"
conf="$conf_dir/config.toml"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script is for macOS only." >&2
  exit 1
fi
if [[ ${EUID} -ne 0 || -z "${SUDO_USER:-}" ]]; then
  echo "Run it with sudo from a normal user: sudo $0" >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

unload() {
  # Ignore the error when the service is not loaded.
  launchctl bootout "system/$label" 2>/dev/null || true
}

if [[ "${1:-}" == "--uninstall" ]]; then
  unload
  rm -f "$plist" "$bin"
  echo "Removed $plist and $bin."
  echo "The configuration $conf stays. Remove it by hand if you do not need it."
  exit 0
fi
if [[ $# -gt 0 ]]; then
  echo "Usage: sudo $0 [--uninstall]" >&2
  exit 1
fi

if ! sudo -u "$SUDO_USER" -i command -v cargo >/dev/null 2>&1; then
  echo "User $SUDO_USER has no cargo in the login shell PATH. Install Rust (https://rustup.rs) first." >&2
  exit 1
fi

echo "Build as $SUDO_USER."
sudo -u "$SUDO_USER" -i bash -c 'cd "$1" && cargo build --release' _ "$root"

mkdir -p /usr/local/bin
install -m 0755 -o root -g wheel "$root/target/release/pc-agent" "$bin"

mkdir -p "$conf_dir"
chown root:wheel "$conf_dir"
chmod 0755 "$conf_dir"
if [[ -e "$conf" ]]; then
  echo "Keep the existing $conf."
else
  install -m 0600 -o root -g wheel "$root/packaging/config.example.toml" "$conf"
  echo "Installed the example configuration at $conf."
fi

install -m 0644 -o root -g wheel "$root/packaging/pc-agent.plist" "$plist"

unload
launchctl bootstrap system "$plist"

cat <<NEXT

Installed. Next steps:
  1. Edit $conf (broker address, user, password, device names).
  2. sudo launchctl kickstart -k system/$label
  3. Log:    tail -f /var/log/pc-agent.log
  4. Status: sudo launchctl print system/$label
NEXT
