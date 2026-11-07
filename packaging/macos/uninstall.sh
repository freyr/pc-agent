#!/bin/bash
# Remove pc-agent that the installer package installed.
#
# Usage: sudo /usr/local/share/pc-agent/uninstall.sh [--purge]
# Stops the service and removes the plist, the binary and /usr/local/share/pc-agent.
# Keeps /etc/pc-agent/config.toml. --purge also removes /etc/pc-agent and the log.

set -euo pipefail

label="pc-agent"
identifier="com.github.freyr.pc-agent"

purge=0
case "${1:-}" in
  "") ;;
  --purge) purge=1 ;;
  *)
    echo "Usage: sudo $0 [--purge]" >&2
    exit 1
    ;;
esac
if [[ ${EUID} -ne 0 ]]; then
  echo "Run it with sudo: sudo $0" >&2
  exit 1
fi

# Ignore the error when the service is not loaded.
launchctl bootout "system/$label" 2>/dev/null || true
# bootout returns before launchd has removed the service. Wait for the removal.
stopped=0
for _ in $(seq 1 50); do
  if ! launchctl print "system/$label" >/dev/null 2>&1; then
    stopped=1
    break
  fi
  sleep 0.2
done
if [[ $stopped -eq 0 ]]; then
  echo "The service system/$label did not stop within 10 s." >&2
  exit 1
fi

rm -f /Library/LaunchDaemons/pc-agent.plist /usr/local/bin/pc-agent
pkgutil --forget "$identifier" >/dev/null 2>&1 || true
echo "Removed the service, the binary and the plist."

if [[ $purge -eq 1 ]]; then
  rm -rf /etc/pc-agent
  rm -f /var/log/pc-agent.log
  echo "Removed /etc/pc-agent and /var/log/pc-agent.log."
else
  echo "The configuration /etc/pc-agent/config.toml stays. Remove it by hand, or run again with --purge."
fi
# Remove the folder of this script last.
rm -rf /usr/local/share/pc-agent
