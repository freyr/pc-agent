#!/usr/bin/env bash
# Install pc-agent as a systemd service on Linux: build it from a clone, or
# take the prebuilt binary of an extracted release archive.
#
# Usage:
#   sudo scripts/install.sh              install (build first in a clone)
#   sudo scripts/install.sh --uninstall  stop the service, remove the unit and the binary
#
# The build runs as the calling user (SUDO_USER). The configuration file
# /etc/pc-agent/config.toml is never overwritten and never removed.

set -euo pipefail

bin="/usr/local/bin/pc-agent"
unit="/etc/systemd/system/pc-agent.service"
conf_dir="/etc/pc-agent"
conf="$conf_dir/config.toml"

# Prints "clone" (a repo with Cargo.toml), "archive" (a prebuilt pc-agent)
# or "none" for the folder $1.
source_kind() {
  if [[ -f "$1/Cargo.toml" ]]; then
    echo clone
  elif [[ -f "$1/pc-agent" ]]; then
    echo archive
  else
    echo none
  fi
}

# Succeeds when the configuration $1 no longer holds the example placeholder.
config_ready() {
  ! grep -q '<broker address>' "$1"
}

main() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    echo "This script is for Linux only." >&2
    exit 1
  fi
  if [[ ${EUID} -ne 0 || -z "${SUDO_USER:-}" ]]; then
    echo "Run it with sudo from a normal user: sudo $0" >&2
    exit 1
  fi

  local root
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

  if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl disable --now pc-agent 2>/dev/null || true
    rm -f "$unit" "$bin"
    systemctl daemon-reload
    echo "Removed $unit and $bin."
    echo "The configuration $conf stays. Remove it by hand if you do not need it."
    exit 0
  fi
  if [[ $# -gt 0 ]]; then
    echo "Usage: sudo $0 [--uninstall]" >&2
    exit 1
  fi

  local src
  case "$(source_kind "$root")" in
    clone)
      # A clone of the repo: build as the calling user, so target/ stays theirs.
      if ! sudo -u "$SUDO_USER" -i command -v cargo >/dev/null 2>&1; then
        echo "User $SUDO_USER has no cargo in the login shell PATH. Install Rust (https://rustup.rs) first." >&2
        exit 1
      fi
      echo "Build as $SUDO_USER."
      # sudo -i passes the command to a login shell, so no shell variable may be
      # left for that shell to expand.
      sudo -u "$SUDO_USER" -i cargo build --release --manifest-path "$root/Cargo.toml"
      src="$root/target/release/pc-agent"
      ;;
    archive)
      # An extracted release archive: install its prebuilt binary.
      src="$root/pc-agent"
      ;;
    *)
      echo "Found neither $root/Cargo.toml nor a prebuilt $root/pc-agent." >&2
      exit 1
      ;;
  esac

  install -D -m 0755 -o root -g root "$src" "$bin"

  install -d -m 0755 -o root -g root "$conf_dir"
  if [[ -e "$conf" ]]; then
    echo "Keep the existing $conf."
  else
    install -m 0600 -o root -g root "$root/packaging/config.example.toml" "$conf"
    echo "Installed the example configuration at $conf."
  fi

  install -m 0644 -o root -g root "$root/packaging/pc-agent.service" "$unit"
  systemctl daemon-reload
  systemctl enable pc-agent

  if config_ready "$conf"; then
    systemctl restart pc-agent
    cat <<DONE

Installed and started.
  Status: systemctl status pc-agent
  Log:    journalctl -u pc-agent -f
  Check:  sudo pc-agent diagnose
DONE
  else
    cat <<NEXT

Installed. The service is enabled but not started. Next steps:
  1. Edit $conf (broker address, user, password, device names).
  2. sudo systemctl restart pc-agent
  3. Log: journalctl -u pc-agent -f
NEXT
  fi
}

# Sourcing the script (for a test) defines the functions only.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
