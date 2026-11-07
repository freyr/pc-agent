# pc-agent

Linux system agent that reports PC power, session and activity state to Home Assistant over MQTT.

It publishes the same MQTT discovery entities as [HASS.Agent](https://github.com/hass-agent/HASS.Agent),
so a dual-boot PC shows as one Home Assistant device on Windows and on Linux. The target is
Omarchy (Arch Linux, Hyprland) with an NVIDIA GPU.

The agent is under construction. The repo holds the probe script below and the Cargo package.

## Probe the target PC

`scripts/probe.sh` reads the signals that the agent depends on and prints them as plain text:

- the Omarchy version and the installed packages;
- the logind sessions;
- the Hyprland monitor state (lock and display), read as root through the user's socket;
- the DRM `dpms` files;
- the NVIDIA driver, and the memory and CPU cost of NVML;
- the input devices, and the gamepad axis limits with 10 s of readings at rest.

It needs bash. It uses `python3` and `jq` if they are present. The output holds user names and
device names but no secret. Do not commit the output files (`probe-*.txt` is git-ignored).

Run it on the target PC, twice:

1. Get the repo:

   ```sh
   git clone https://github.com/freyr/pc-agent.git
   cd pc-agent
   ```

2. Run the probe while the PC is unlocked. Do not touch the gamepad while it runs.

   ```sh
   sudo bash scripts/probe.sh > probe-unlocked.txt 2>&1
   ```

3. Run the probe while the PC is locked. The first command asks for the password, then the probe
   starts 20 s later in the background:

   ```sh
   sudo -v; (sleep 20; sudo bash scripts/probe.sh > probe-locked.txt 2>&1) &
   ```

   Lock the PC at once. Unlock it after 30 s.

4. Send `probe-unlocked.txt` and `probe-locked.txt` to the maintainer.
