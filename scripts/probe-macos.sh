#!/bin/bash
# Samples the macOS lock, display and idle signals every 2 s for 120 s.
# Run it, then lock the screen and let the display sleep (pmset displaysleepnow),
# wait 30 s, wake the Mac and unlock it. Send the output file.
# Usage: bash scripts/probe-macos.sh > probe-macos.txt 2>&1
set -u

echo "# sw_vers"; sw_vers
echo "# user: $(id -un), euid: $(id -u)"
echo "# columns: time locked_key framebuffer_power_states backlight_raw idle_s user_is_active"

end=$((SECONDS + ${1:-120}))
while [ "$SECONDS" -lt "$end" ]; do
  root=$(ioreg -n Root -d1)
  locked=$(echo "$root" | grep -o '"CGSSessionScreenIsLocked"=[A-Za-z]*' | head -1)
  [ -z "$locked" ] && locked="absent"
  fb=$(ioreg -r -c IOMobileFramebufferShim -d1 | grep -o '"CurrentPowerState"=[0-9]*' | cut -d= -f2 | tr '\n' ',')
  bl=$(ioreg -r -c AppleARMBacklight -d1 | grep -o '"rawBrightness"={[^}]*}' | grep -o '"value"=[0-9]*' | cut -d= -f2)
  idle=$(ioreg -c IOHIDSystem -d4 | grep -o '"HIDIdleTime" = [0-9]*' | head -1 | awk '{print int($3/1000000000)}')
  active=$(pmset -g assertions | awk '/UserIsActive/ {print $2; exit}')
  echo "$(date +%H:%M:%S) locked=$locked fb=$fb backlight=${bl:-none} idle=${idle}s userisactive=${active:-none}"
  sleep 2
done
