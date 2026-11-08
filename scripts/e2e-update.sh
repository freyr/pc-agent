#!/usr/bin/env bash
# CI end-to-end test of `pc-agent update` and of the rollback, on Linux
# (systemd) and macOS (launchd). It changes the system: it installs the
# service pc-agent, /usr/local/bin/pc-agent and /etc/pc-agent/config.toml,
# and removes them at the end. Run it only on a CI runner.
#
# Usage: bash scripts/e2e-update.sh
# Needs sudo, cargo, minisign, mosquitto (broker and clients) and python3.
#
#   1. Makes a throwaway minisign key and builds the agent three times with
#      the test feature `update-test-key` (the key and the version are set
#      at build time, PC_AGENT_UPDATE_URL at run time): 0.0.1, 0.0.2, and
#      0.0.3 with `update-test-exit` (it exits after the pending-file check).
#   2. Runs a broker and a release server (the `stable` URLs) on 127.0.0.1.
#   3. Installs 0.0.1 as the service; it must publish sw_version 0.0.1.
#   4. Serves a signed manifest of 0.0.2 and runs `pc-agent update`: the
#      service must publish sw_version 0.0.2, and the first ConnAck must
#      delete update-pending.json and pc-agent.prev.
#   5. Serves 0.0.3 and runs `pc-agent update`: after the third start with
#      no ConnAck, 0.0.2 must be back at the binary path and publish again,
#      and update-state.json must record the commit of 0.0.3 as failed.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d)"
bin=/usr/local/bin/pc-agent
conf=/etc/pc-agent/config.toml
port=18080
url="http://127.0.0.1:$port"
config_topic=homeassistant/sensor/CI-TEST/lastboot/config
commit2=2222222222222222222222222222222222222222
commit3=3333333333333333333333333333333333333333

case "$(uname -s)" in
  Linux)
    platform=x86_64-linux
    asset=pc-agent-x86_64-linux
    state=/var/lib/pc-agent
    broker=mosquitto
    ;;
  Darwin)
    platform=aarch64-macos
    asset=pc-agent-aarch64-macos
    state="/Library/Application Support/pc-agent"
    broker="$(brew --prefix mosquitto)/sbin/mosquitto"
    ;;
  *)
    echo "unsupported OS $(uname -s)" >&2
    exit 1
    ;;
esac

service_stop() {
  if [[ "$platform" == x86_64-linux ]]; then
    sudo systemctl stop pc-agent 2>/dev/null || true
    sudo rm -f /etc/systemd/system/pc-agent.service
    sudo systemctl daemon-reload
  else
    sudo launchctl bootout system/pc-agent 2>/dev/null || true
    sudo rm -f /Library/LaunchDaemons/pc-agent.plist
  fi
}

show_logs() {
  echo "== service log"
  if [[ "$platform" == x86_64-linux ]]; then
    sudo journalctl -u pc-agent --no-pager -n 200 || true
  else
    sudo tail -n 200 /var/log/pc-agent.log || true
  fi
  echo "== state folder"
  sudo ls -la "$state" || true
  sudo sh -c "cat '$state'/update-*.json" || true
  echo "== binary folder"
  ls -la /usr/local/bin | grep pc-agent || true
}

cleanup() {
  local code=$?
  if [[ $code -ne 0 ]]; then
    show_logs
  fi
  service_stop
  sudo rm -rf "$bin" "$bin.prev" /etc/pc-agent "$state"
  [[ -n "${http_pid:-}" ]] && kill "$http_pid" 2>/dev/null || true
  [[ -f "$work/mosquitto.pid" ]] && kill "$(cat "$work/mosquitto.pid")" 2>/dev/null || true
  rm -rf "$work"
  exit "$code"
}
trap cleanup EXIT

fail() {
  echo "::error::$*" >&2
  exit 1
}

sha256() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# 1. The key and the three builds.
minisign -G -W -p "$work/test.pub" -s "$work/test.key" > /dev/null
key="$(tail -n 1 "$work/test.pub")"
build() {
  local version="$1" features="$2"
  # No debug info: a debug binary with it is larger than the download limit.
  CARGO_PROFILE_DEV_DEBUG=0 PC_AGENT_TEST_VERSION="$version" PC_AGENT_TEST_PUBLIC_KEY="$key" \
    cargo build --quiet --features "$features" --target-dir "$root/target/e2e"
  cp "$root/target/e2e/debug/pc-agent" "$work/pc-agent-$version"
  # No server runs yet: the check fails after it prints the running build.
  local out
  out="$(PC_AGENT_UPDATE_URL="$url" "$work/pc-agent-$version" update --check --channel stable 2>&1 || true)"
  [[ "$out" == "running: $version "* ]] || fail "the build $version does not have its version: $out"
}
build 0.0.1 update-test-key
build 0.0.2 update-test-key
build 0.0.3 update-test-key,update-test-exit

# 2. The broker and the release server.
if [[ "$platform" == x86_64-linux ]]; then
  # The package starts its own broker on 1883.
  sudo systemctl stop mosquitto 2>/dev/null || true
fi
cat > "$work/mosquitto.conf" <<EOF
listener 1883 127.0.0.1
allow_anonymous true
pid_file $work/mosquitto.pid
EOF
"$broker" -c "$work/mosquitto.conf" -d
mkdir -p "$work/www/latest/download"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$work/www" > "$work/http.log" 2>&1 &
http_pid=$!
sleep 2

# The signed `stable` release of one build.
release() {
  local version="$1" commit="$2" file="$3"
  local dir="$work/www/latest/download"
  cp "$file" "$dir/$asset"
  cat > "$dir/manifest.json" <<EOF
{
  "version": "$version",
  "commit": "$commit",
  "built_at": "2026-11-08T12:00:00Z",
  "platforms": {
    "$platform": { "asset": "$asset", "sha256": "$(sha256 "$file")" }
  }
}
EOF
  rm -f "$dir/manifest.json.minisig"
  minisign -S -s "$work/test.key" -m "$dir/manifest.json" -x "$dir/manifest.json.minisig" > /dev/null
}

# The sw_version of the retained discovery config, or nothing.
sw_version() {
  mosquitto_sub -h 127.0.0.1 -t "$config_topic" -C 1 -W 3 2>/dev/null \
    | sed -n 's/.*"sw_version":"\([^"]*\)".*/\1/p' || true
}

wait_version() {
  local expected="$1" seconds="$2" got=""
  for _ in $(seq 1 "$seconds"); do
    got="$(sw_version)"
    [[ "$got" == "$expected" ]] && echo "sw_version: $got" && return 0
    sleep 1
  done
  fail "sw_version is '$got', expected $expected within $seconds s"
}

update() {
  sudo env PC_AGENT_UPDATE_URL="$url" "$bin" update --channel stable
}

# 3. The old build as the service.
sudo mkdir -p /usr/local/bin /etc/pc-agent
sudo install -m 0755 "$work/pc-agent-0.0.1" "$bin"
sudo tee "$conf" > /dev/null <<EOF
[mqtt]
host = "127.0.0.1"
port = 1883
username = "u"
password = "p"

[device]
name = "CI-TEST"
identifier = "pc-agent-CI-TEST"
entity_prefix = "ci_test"

[gpu]
enabled = false
EOF
if [[ "$platform" == x86_64-linux ]]; then
  sudo install -m 0644 "$root/packaging/pc-agent.service" /etc/systemd/system/pc-agent.service
  sudo systemctl daemon-reload
  sudo systemctl start pc-agent
else
  sudo install -m 0644 "$root/packaging/pc-agent.plist" /Library/LaunchDaemons/pc-agent.plist
  sudo launchctl bootstrap system /Library/LaunchDaemons/pc-agent.plist
fi
wait_version 0.0.1 60

# 4. The update to 0.0.2.
release 0.0.2 "$commit2" "$work/pc-agent-0.0.2"
update
cmp -s "$bin" "$work/pc-agent-0.0.2" || fail "the binary is not 0.0.2 after the install"
wait_version 0.0.2 90
confirmed=false
for _ in $(seq 1 30); do
  if ! sudo test -e "$state/update-pending.json" && ! test -e "$bin.prev"; then
    confirmed=true
    break
  fi
  sleep 1
done
$confirmed || fail "update-pending.json or pc-agent.prev is still there after the ConnAck"
echo "OK: 0.0.2 runs and the update is confirmed"

# A second run finds no newer build.
update | tee "$work/again.txt"
grep -q '^up to date$' "$work/again.txt" || fail "a second update did not print 'up to date'"

# 5. The broken build 0.0.3 and the rollback.
mosquitto_pub -h 127.0.0.1 -r -n -t "$config_topic"
release 0.0.3 "$commit3" "$work/pc-agent-0.0.3"
update
cmp -s "$bin" "$work/pc-agent-0.0.3" || fail "the binary is not 0.0.3 after the install"
rolled_back=false
for _ in $(seq 1 180); do
  if sudo test -e "$state/update-state.json" \
    && sudo grep -q "$commit3" "$state/update-state.json" \
    && ! sudo test -e "$state/update-pending.json"; then
    rolled_back=true
    break
  fi
  sleep 1
done
$rolled_back || fail "no rollback of 0.0.3 within 180 s"
sudo cat "$state/update-state.json"
cmp -s "$bin" "$work/pc-agent-0.0.2" || fail "the binary is not 0.0.2 after the rollback"
# The retained config was cleared: only the old build can publish it again.
wait_version 0.0.2 90
echo "OK: 0.0.3 failed, 0.0.2 is back and the failed build is recorded"

# The failed build is offered again only to a manual update (U4 adds the
# automatic path); --check reports it.
sudo env PC_AGENT_UPDATE_URL="$url" "$bin" update --check --channel stable | tee "$work/check.txt"
grep -q "^newer build failed before: 0.0.3 commit $commit3" "$work/check.txt" \
  || fail "update --check does not report the failed build"
echo "The update end-to-end test passed."
