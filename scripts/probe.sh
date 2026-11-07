#!/usr/bin/env bash
# Probe the signals that pc-agent depends on, on the target PC.
#
# Run as root, once while unlocked and once while locked:
#   sudo bash scripts/probe.sh > probe-unlocked.txt 2>&1
#   sudo -v; (sleep 20; sudo bash scripts/probe.sh > probe-locked.txt 2>&1) &
#   # lock the PC at once, unlock it after 30 s
#
# Do not touch the gamepad while the script runs (section 6 reads it at rest
# for 10 s). The output holds user names and device names but no secret.
# Do not commit the output files.

set -u
shopt -s nullglob

if [[ ${EUID} -ne 0 ]]; then
  echo "Run as root: sudo bash $0" >&2
  exit 1
fi

have() { command -v "$1" >/dev/null 2>&1; }
section() { printf '\n===== %s =====\n' "$1"; }
run() { printf '$ %s\n' "$*"; timeout 15 "$@" 2>&1; printf '(exit %s)\n' "$?"; }

section "0. Probe"
date -u '+%Y-%m-%d %H:%M:%SZ'
uname -r
for tool in python3 jq hyprctl nvidia-smi udevadm busctl; do
  if have "$tool"; then echo "$tool: $(command -v "$tool")"; else echo "$tool: missing"; fi
done

section "1. Omarchy version and packages"
for f in /usr/share/omarchy/version /home/*/.local/share/omarchy/version; do
  [[ -f $f ]] && echo "$f: $(cat "$f")"
done
for d in /home/*/.local/share/omarchy; do
  [[ -d $d/.git ]] && echo "$d git: $(git -C "$d" describe --tags --always 2>&1)"
done
have omarchy-version && run omarchy-version
run pacman -Q hyprland hypridle hyprlock quickshell sddm uwsm nvidia-open-dkms nvidia-open nvidia-utils networkmanager iwd
cat /etc/sddm.conf.d/*.conf 2>/dev/null | grep -vi password

section "2. logind sessions"
run loginctl list-sessions --no-legend
for s in $(loginctl list-sessions --no-legend | awk '{print $1}'); do
  echo "--- session $s"
  loginctl show-session "$s" -p Id -p Name -p User -p Class -p Type -p Seat \
    -p Active -p Remote -p State -p Service -p LockedHint -p IdleHint 2>&1
done
run busctl get-property org.freedesktop.login1 /org/freedesktop/login1 \
  org.freedesktop.login1.Manager InhibitDelayMaxUSec
run systemctl show -p DefaultTimeoutStopUSec
run systemctl is-active NetworkManager systemd-networkd iwd

section "3. Hyprland instances (as root)"
for dir in /run/user/*/hypr/*/; do
  [[ -d $dir ]] || continue
  uid=$(echo "$dir" | cut -d/ -f4)
  sig=$(basename "$dir")
  user=$(getent passwd "$uid" | cut -d: -f1)
  echo "--- uid=$uid user=$user instance=$sig"
  ls -la --time-style=+%Y-%m-%dT%H:%M:%S "$dir" 2>&1
  stat -c '%a %U %n' "$(dirname "$dir")" 2>&1
  if have hyprctl; then
    out=$(XDG_RUNTIME_DIR=/run/user/$uid HYPRLAND_INSTANCE_SIGNATURE=$sig \
      timeout 5 hyprctl -j monitors all 2>&1)
    echo "hyprctl exit: $?"
    if have jq && echo "$out" | jq -e . >/dev/null 2>&1; then
      echo "$out" | jq -c '.[] | {name, disabled, dpmsStatus, solitary, solitaryBlockedBy}'
    else
      echo "$out" | head -c 4000; echo
    fi
    XDG_RUNTIME_DIR=/run/user/$uid HYPRLAND_INSTANCE_SIGNATURE=$sig \
      timeout 5 hyprctl version 2>&1 | head -3
  fi
done
echo "--- lock processes"
pgrep -a -x 'quickshell|qs|hyprlock|hypridle' 2>&1

section "4. DRM connectors"
for c in /sys/class/drm/card*-*; do
  [[ -f $c/status ]] || continue
  printf '%s status=%s enabled=%s dpms=%s\n' "$(basename "$c")" \
    "$(cat "$c/status")" "$(cat "$c/enabled" 2>/dev/null)" "$(cat "$c/dpms" 2>/dev/null)"
done

section "5. GPU"
run cat /proc/driver/nvidia/version
for f in /usr/lib/libnvidia-ml.so*; do ls -la "$f"; done
have nvidia-smi && run nvidia-smi --query-gpu=name,driver_version,utilization.gpu,power.draw,pstate --format=csv
for card in /sys/class/drm/card[0-9]; do
  echo "$(basename "$card") vendor=$(cat "$card/device/vendor" 2>/dev/null) busy=$(cat "$card/device/gpu_busy_percent" 2>/dev/null || echo none)"
done
for h in /sys/class/hwmon/hwmon*; do
  files=()
  for f in "$h"/{power,temp,in,curr}[0-9]*_{input,average,label}; do
    [[ -e $f ]] && files+=("$(basename "$f")")
  done
  echo "$(basename "$h") name=$(cat "$h/name" 2>/dev/null): ${files[*]:-}"
done

if have python3; then
  echo "--- NVML resident memory and query cost"
  python3 -I - <<'PY'
import ctypes, os, resource, time

def rss():
    with open("/proc/self/status") as f:
        for line in f:
            if line.startswith("VmRSS"):
                return int(line.split()[1])

def cpu():
    r = resource.getrusage(resource.RUSAGE_SELF)
    return r.ru_utime + r.ru_stime

base = rss()
try:
    nvml = ctypes.CDLL("libnvidia-ml.so.1")
except OSError as e:
    print("load failed:", e)
    raise SystemExit
after_load = rss()
rc = nvml.nvmlInit_v2()
after_init = rss()
print(f"VmRSS kB: base={base} after_load={after_load} after_init={after_init} (init rc={rc})")
if rc == 0:
    handle = ctypes.c_void_p()
    print("handle rc:", nvml.nvmlDeviceGetHandleByIndex_v2(0, ctypes.byref(handle)))

    class Util(ctypes.Structure):
        _fields_ = [("gpu", ctypes.c_uint), ("memory", ctypes.c_uint)]

    util, mw = Util(), ctypes.c_uint()
    c0, t0 = cpu(), time.monotonic()
    for _ in range(100):
        nvml.nvmlDeviceGetUtilizationRates(handle, ctypes.byref(util))
        nvml.nvmlDeviceGetPowerUsage(handle, ctypes.byref(mw))
    c1, t1 = cpu(), time.monotonic()
    print(f"gpu={util.gpu}% power={mw.value / 1000:.1f}W")
    print(f"100 query pairs: cpu={1000 * (c1 - c0):.1f} ms wall={1000 * (t1 - t0):.1f} ms")
    print(f"VmRSS kB after queries: {rss()}")
    nvml.nvmlShutdown()
PY
fi

section "6. Input devices"
for ev in /sys/class/input/event*; do
  n=$(basename "$ev")
  props=$(udevadm info -q property -n "/dev/input/$n" 2>/dev/null | grep -E '^ID_INPUT[A-Z_]*=1' | sed 's/=1//' | tr '\n' ' ')
  echo "$n name=\"$(cat "$ev/device/name" 2>/dev/null)\" props=$(cat "$ev/device/properties" 2>/dev/null) $props"
done

if have python3; then
  echo "--- gamepad axes at rest (10 s, do not touch)"
  python3 -I - <<'PY'
import fcntl, glob, os, select, struct, time

ABS_NAMES = {0: "X", 1: "Y", 2: "Z", 3: "RX", 4: "RY", 5: "RZ", 16: "HAT0X", 17: "HAT0Y"}

def bits(path):
    words = open(path).read().split()
    out = set()
    for i, w in enumerate(reversed(words)):
        v = int(w, 16)
        for b in range(64):
            if v >> b & 1:
                out.add(i * 64 + b)
    return out

pads = []
for ev in sorted(glob.glob("/sys/class/input/event*")):
    keys = bits(ev + "/device/capabilities/key")
    if not any(0x120 <= k < 0x140 for k in keys):
        continue
    if int(open(ev + "/device/properties").read().split()[-1], 16) >> 6 & 1:
        continue  # INPUT_PROP_ACCELEROMETER
    axes = sorted(bits(ev + "/device/capabilities/abs"))
    pads.append((ev, axes))

if not pads:
    print("no gamepad found")

fds = {}
for ev, axes in pads:
    dev = "/dev/input/" + os.path.basename(ev)
    fd = os.open(dev, os.O_RDONLY | os.O_NONBLOCK)
    print(f"{dev} \"{open(ev + '/device/name').read().strip()}\"")
    for a in axes:
        buf = fcntl.ioctl(fd, 0x80184540 + a, b"\0" * 24)
        value, lo, hi, fuzz, flat, res = struct.unpack("6i", buf)
        print(f"  ABS_{ABS_NAMES.get(a, a)}: min={lo} max={hi} fuzz={fuzz} flat={flat} now={value}")
    fds[fd] = (dev, {})

end = time.monotonic() + 10
while fds and time.monotonic() < end:
    r, _, _ = select.select(list(fds), [], [], max(0, end - time.monotonic()))
    for fd in r:
        try:
            data = os.read(fd, 24 * 64)
        except BlockingIOError:
            continue
        for i in range(0, len(data) - 23, 24):
            _, _, etype, code, value = struct.unpack("qqHHi", data[i:i + 24])
            if etype == 3:  # EV_ABS
                seen = fds[fd][1].setdefault(code, [0, value, value])
                seen[0] += 1
                seen[1] = min(seen[1], value)
                seen[2] = max(seen[2], value)
for fd, (dev, seen) in fds.items():
    print(f"{dev} events at rest:", {f"ABS_{ABS_NAMES.get(c, c)}": f"n={n} range={lo}..{hi}" for c, (n, lo, hi) in seen.items()} or "none")
    os.close(fd)
PY
fi

section "end"
